use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::snapshot::{
    SnapshotAgentMark, SnapshotAgentMarkInput, SnapshotPruneOptions, SnapshotPruneResult,
    SnapshotRecord, SnapshotRecordInput, SnapshotStore, is_snapshot_agent_mark,
    is_snapshot_record, SNAPSHOT_RECORD_VERSION,
};
use crate::rpc::types::BoxFuture;

const LOCK_RETRY: Duration = Duration::from_millis(25);
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_HEARTBEAT: Duration = Duration::from_secs(10);

fn create_exclusive() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalized_path(path: &str) -> String {
    let slashed = path.replace('\\', "/");
    let bytes = slashed.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return slashed.to_lowercase();
    }
    slashed
}

pub fn snapshot_path_key(path: &str) -> String {
    sha256_hex(normalized_path(path).as_bytes())
}

pub fn snapshot_content_hash(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

pub fn encode_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

struct FileLockGuard {
    path: PathBuf,
    token: String,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl FileLockGuard {
    async fn acquire(root: &Path) -> Result<Self, String> {
        fs::create_dir_all(root)
            .await
            .map_err(|error| format!("create snapshot store root {}: {error}", root.display()))?;
        let path = root.join(".lock");
        let token = Uuid::new_v4().to_string();
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match fs::OpenOptions::from(create_exclusive()).open(&path).await {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(token.as_bytes()).await {
                        return Err(format!("write snapshot lock token: {error}"));
                    }
                    break;
                }
                Err(error) => {
                    let kind = error.kind();
                    if kind != std::io::ErrorKind::AlreadyExists
                        && kind != std::io::ErrorKind::PermissionDenied
                    {
                        return Err(format!("take snapshot lock {}: {error}", path.display()));
                    }
                    if kind == std::io::ErrorKind::PermissionDenied && !path.exists() {
                        return Err(format!("take snapshot lock {}: {error}", path.display()));
                    }
                    let age = match fs::metadata(&path).await {
                        Ok(info) => info
                            .modified()
                            .ok()
                            .and_then(|modified| modified.elapsed().ok())
                            .unwrap_or_default(),
                        Err(_) => Duration::ZERO,
                    };
                    if age > LOCK_STALE {
                        let _ = fs::remove_file(&path).await;
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(format!(
                            "Timed out waiting for the snapshot lock: {}",
                            path.display()
                        ));
                    }
                    tokio::time::sleep(LOCK_RETRY).await;
                }
            }
        }
        let heartbeat_path = path.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(LOCK_HEARTBEAT).await;
                if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&heartbeat_path) {
                    let now = SystemTime::now();
                    let _ = file.set_times(std::fs::FileTimes::new().set_modified(now));
                }
            }
        });
        Ok(Self {
            path,
            token,
            heartbeat: Some(heartbeat),
        })
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        let owner = std::fs::read_to_string(&self.path).unwrap_or_default();
        if owner == self.token {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct FileSnapshotStore {
    root: PathBuf,
    objects_dir: PathBuf,
    paths_dir: PathBuf,
    agents_dir: PathBuf,
    lock: Mutex<()>,
}

impl FileSnapshotStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            objects_dir: root.join("objects"),
            paths_dir: root.join("paths"),
            agents_dir: root.join("agents"),
            root,
            lock: Mutex::new(()),
        }
    }

    async fn write_atomic(&self, path: &Path, data: &str) -> Result<(), String> {
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record");
        let temp = path.with_file_name(format!("{stem}.{}.tmp", &Uuid::new_v4().to_string()[..8]));
        fs::write(&temp, data)
            .await
            .map_err(|error| format!("write {}: {error}", temp.display()))?;
        fs::rename(&temp, path)
            .await
            .map_err(|error| format!("rename {} to {}: {error}", temp.display(), path.display()))?;
        Ok(())
    }

    fn path_dir(&self, path: &str) -> PathBuf {
        let key = snapshot_path_key(path);
        self.paths_dir.join(&key[..2]).join(&key)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        let bucket = hash.chars().take(2).collect::<String>();
        self.objects_dir.join(bucket).join(hash)
    }

    async fn readdir_safe(&self, dir: &Path) -> Result<Vec<String>, String> {
        let mut entries = match fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("read directory {}: {error}", dir.display())),
        };
        let mut names = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| format!("read directory {}: {error}", dir.display()))?
        {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    async fn unlink_safe(&self, path: &Path) -> Result<bool, String> {
        match fs::remove_file(path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        }
    }

    async fn read_json(&self, path: &Path) -> Option<serde_json::Value> {
        let raw = fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&raw).ok()
    }

    async fn newest_id(&self, dir: &Path) -> Result<Option<String>, String> {
        let mut newest: Option<String> = None;
        for name in self.readdir_safe(dir).await? {
            if !name.ends_with(".json") {
                continue;
            }
            let id = name.trim_end_matches(".json");
            if newest.as_deref().is_none_or(|current| id > current) {
                newest = Some(id.to_string());
            }
        }
        Ok(newest)
    }

    async fn path_dirs(&self) -> Result<Vec<PathBuf>, String> {
        let mut dirs = Vec::new();
        for bucket in self.readdir_safe(&self.paths_dir).await? {
            let bucket_dir = self.paths_dir.join(&bucket);
            for key in self.readdir_safe(&bucket_dir).await? {
                dirs.push(bucket_dir.join(key));
            }
        }
        Ok(dirs)
    }

    async fn object_dirs(&self) -> Result<Vec<PathBuf>, String> {
        Ok(self
            .readdir_safe(&self.objects_dir)
            .await?
            .into_iter()
            .map(|bucket| self.objects_dir.join(bucket))
            .collect())
    }

    async fn write_object(&self, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let target = self.object_path(hash);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await.map_err(|error| {
                format!("create snapshot object dir {}: {error}", parent.display())
            })?;
        }
        if target.exists() {
            if self.content(hash.to_string()).await?.is_some() {
                return Ok(());
            }
            self.unlink_safe(&target).await?;
        }
        let stem = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("object");
        let temp = target.with_file_name(format!("{stem}.{}.tmp", Uuid::new_v4()));
        match fs::OpenOptions::from(create_exclusive()).open(&temp).await {
            Ok(mut file) => {
                file.write_all(bytes)
                    .await
                    .map_err(|error| format!("write {}: {error}", temp.display()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temp).await;
                return Ok(());
            }
            Err(error) => return Err(format!("write {}: {error}", temp.display())),
        }
        match fs::rename(&temp, &target).await {
            Ok(()) => {}
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "rename {} to {}: {error}",
                    temp.display(),
                    target.display()
                ));
            }
            Err(_) => {}
        }
        let _ = fs::remove_file(&temp).await;
        Ok(())
    }

    async fn with_lock<T>(
        &self,
        run: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let _local = self.lock.lock().await;
        let _file = FileLockGuard::acquire(&self.root).await?;
        run.await
    }

    async fn record_inner(&self, input: SnapshotRecordInput) -> Result<SnapshotRecord, String> {
        let dir = self.path_dir(&input.path);
        fs::create_dir_all(&dir)
            .await
            .map_err(|error| format!("create snapshot path dir {}: {error}", dir.display()))?;
        let previous_id = self.newest_id(&dir).await?;
        let bytes = input.bytes.clone().unwrap_or_default();
        let hash = if input.bytes.is_some() {
            snapshot_content_hash(&bytes)
        } else {
            String::new()
        };
        if !hash.is_empty() {
            self.write_object(&hash, &bytes).await?;
        }
        let at_ms = chrono::DateTime::parse_from_rfc3339(&input.at)
            .map(|at| at.timestamp_millis())
            .unwrap_or(0);
        let id = format!("{at_ms:013}-{}", &Uuid::new_v4().to_string()[..8]);
        let record = SnapshotRecord {
            version: SNAPSHOT_RECORD_VERSION,
            id,
            path: input.path,
            hash,
            size: input.size,
            mtime_ms: input.mtime_ms,
            at: input.at,
            tool: input.tool,
            mode: input.mode,
            session_id: input.session_id,
            agent_id: input.agent_id,
            previous_id,
            skipped: input.skipped,
            after_hash: None,
            after_size: None,
        };
        let file = dir.join(format!("{}.json", record.id));
        self.write_atomic(
            &file,
            &format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .await?;
        Ok(record)
    }

    async fn list_inner(&self, path: String) -> Result<Vec<SnapshotRecord>, String> {
        let dir = self.path_dir(&path);
        let mut records = Vec::new();
        for name in self.readdir_safe(&dir).await? {
            if !name.ends_with(".json") {
                continue;
            }
            if let Some(value) = self.read_json(&dir.join(name)).await {
                if is_snapshot_record(&value)
                    && let Ok(record) = serde_json::from_value::<SnapshotRecord>(value)
                {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left, right| right.id.cmp(&left.id));
        Ok(records)
    }

    async fn content_inner(&self, hash: String) -> Result<Option<Vec<u8>>, String> {
        if hash.is_empty() {
            return Ok(None);
        }
        let path = self.object_path(&hash);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .await
            .map_err(|error| format!("read snapshot object {}: {error}", path.display()))?;
        Ok(if sha256_hex(&bytes) == hash {
            Some(bytes)
        } else {
            None
        })
    }

    async fn prune_inner(&self, options: SnapshotPruneOptions) -> Result<SnapshotPruneResult, String> {
        let oldest_kept = options.now_ms - options.max_age_ms;
        let sweep_started_at = chrono::Utc::now().timestamp_millis();
        let mut referenced = std::collections::HashSet::new();
        let mut result = SnapshotPruneResult::default();
        for dir in self.path_dirs().await? {
            let mut names = self
                .readdir_safe(&dir)
                .await?
                .into_iter()
                .filter(|name| name.ends_with(".json"))
                .collect::<Vec<_>>();
            names.sort();
            names.reverse();
            let mut kept_bytes: u64 = 0;
            for (index, name) in names.iter().enumerate() {
                let record = self
                    .read_json(&dir.join(name))
                    .await
                    .and_then(|value| serde_json::from_value::<SnapshotRecord>(value).ok());
                let Some(record) = record else {
                    if index > 0 && self.unlink_safe(&dir.join(name)).await? {
                        result.records_removed += 1;
                    }
                    continue;
                };
                let expired = matches!(
                    chrono::DateTime::parse_from_rfc3339(&record.at),
                    Ok(at) if at.timestamp_millis() < oldest_kept
                );
                let over_budget =
                    index > 0 && kept_bytes.saturating_add(record.size) > options.max_bytes_per_path;
                if index >= options.max_per_path || (expired && index > 0) || over_budget {
                    if self.unlink_safe(&dir.join(name)).await? {
                        result.records_removed += 1;
                    }
                    continue;
                }
                kept_bytes = kept_bytes.saturating_add(record.size);
                if !record.hash.is_empty() {
                    referenced.insert(record.hash);
                }
            }
        }
        for dir in self.object_dirs().await? {
            for name in self.readdir_safe(&dir).await? {
                if referenced.contains(&name) {
                    continue;
                }
                let path = dir.join(&name);
                match fs::metadata(&path).await {
                    Ok(info) => {
                        let modified_ms = info
                            .modified()
                            .ok()
                            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                            .map(|age| age.as_millis() as i64);
                        if modified_ms.is_some_and(|modified| {
                            sweep_started_at - modified < options.min_object_age_ms
                        }) {
                            continue;
                        }
                    }
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(format!("stat {}: {error}", path.display()));
                    }
                    Err(_) => continue,
                }
                if self.unlink_safe(&path).await? {
                    result.objects_removed += 1;
                }
            }
        }
        for bucket in self.readdir_safe(&self.agents_dir).await? {
            let dir = self.agents_dir.join(&bucket);
            for name in self.readdir_safe(&dir).await? {
                if !name.ends_with(".json") {
                    continue;
                }
                if let Some(value) = self.read_json(&dir.join(&name)).await {
                    if is_snapshot_agent_mark(&value) {
                        let at = value
                            .get("at")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                            .map(|at| at.timestamp_millis());
                        if at.is_some_and(|at| at >= oldest_kept) {
                            continue;
                        }
                    }
                }
                if self.unlink_safe(&dir.join(&name)).await? {
                    result.agent_marks_removed += 1;
                }
            }
        }
        Ok(result)
    }
}

impl SnapshotStore for FileSnapshotStore {
    fn record<'a>(
        &'a self,
        input: SnapshotRecordInput,
    ) -> BoxFuture<'a, Result<SnapshotRecord, String>> {
        Box::pin(async move { self.with_lock(self.record_inner(input)).await })
    }

    fn complete<'a>(
        &'a self,
        path: String,
        id: String,
        after_hash: String,
        after_size: u64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.with_lock(async {
                let file = self.path_dir(&path).join(format!("{id}.json"));
                let Some(value) = self.read_json(&file).await else {
                    return Ok(false);
                };
                let Ok(mut record) = serde_json::from_value::<SnapshotRecord>(value) else {
                    return Ok(false);
                };
                record.after_hash = Some(after_hash);
                record.after_size = Some(after_size);
                self.write_atomic(
                    &file,
                    &format!("{}\n", serde_json::to_string(&record).unwrap()),
                )
                .await?;
                Ok(true)
            })
            .await
        })
    }

    fn remove<'a>(&'a self, path: String, id: String) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            Ok(self
                .unlink_safe(&self.path_dir(&path).join(format!("{id}.json")))
                .await?)
        })
    }

    fn list<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<SnapshotRecord>, String>> {
        Box::pin(async move { self.list_inner(path).await })
    }

    fn content<'a>(&'a self, hash: String) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { self.content_inner(hash).await })
    }

    fn mark_agent<'a>(
        &'a self,
        input: SnapshotAgentMarkInput,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.with_lock(async {
                let dir = self.agents_dir.join(encode_segment(&input.session_id));
                fs::create_dir_all(&dir).await.map_err(|error| {
                    format!("create snapshot agent dir {}: {error}", dir.display())
                })?;
                let mark = SnapshotAgentMark {
                    version: SNAPSHOT_RECORD_VERSION,
                    agent_id: input.agent_id,
                    session_id: input.session_id,
                    at: input.at,
                    cwd: input.cwd,
                    parent_agent_id: input.parent_agent_id,
                    profile_name: input.profile_name,
                    swarm_index: input.swarm_index,
                };
                self.write_atomic(
                    &dir.join(format!("{}.json", encode_segment(&mark.agent_id))),
                    &format!("{}\n", serde_json::to_string(&mark).unwrap()),
                )
                .await?;
                Ok(())
            })
            .await
        })
    }

    fn agents<'a>(
        &'a self,
        session_id: String,
    ) -> BoxFuture<'a, Result<Vec<SnapshotAgentMark>, String>> {
        Box::pin(async move {
            let dir = self.agents_dir.join(encode_segment(&session_id));
            let mut marks = Vec::new();
            for name in self.readdir_safe(&dir).await? {
                if !name.ends_with(".json") {
                    continue;
                }
                if let Some(value) = self.read_json(&dir.join(name)).await {
                    if let Ok(mark) = serde_json::from_value::<SnapshotAgentMark>(value) {
                        marks.push(mark);
                    }
                }
            }
            marks.sort_by(|left, right| left.at.cmp(&right.at));
            Ok(marks)
        })
    }

    fn prune<'a>(
        &'a self,
        options: SnapshotPruneOptions,
    ) -> BoxFuture<'a, Result<SnapshotPruneResult, String>> {
        Box::pin(async move { self.with_lock(self.prune_inner(options)).await })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::snapshot::snapshot::SnapshotWriteMode;

    fn store() -> (tempfile::TempDir, FileSnapshotStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSnapshotStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    fn input(path: &str, bytes: Option<&[u8]>) -> SnapshotRecordInput {
        SnapshotRecordInput {
            path: path.into(),
            at: "2026-01-01T00:00:00.000Z".into(),
            tool: "write".into(),
            mode: SnapshotWriteMode::Overwrite,
            size: bytes.map(|body| body.len() as u64).unwrap_or(0),
            bytes: bytes.map(<[u8]>::to_vec),
            mtime_ms: None,
            session_id: Some("sess-1".into()),
            agent_id: Some("main".into()),
            skipped: None,
        }
    }

    #[test]
    fn path_keys_normalize_separators_and_drive_case() {
        assert_eq!(
            snapshot_path_key(r"C:\repo\src\a.ts"),
            snapshot_path_key("c:/repo/src/a.ts")
        );
        assert_eq!(snapshot_path_key("src/a.ts"), snapshot_path_key("src/a.ts"));
        assert_ne!(snapshot_path_key("src/a.ts"), snapshot_path_key("src/b.ts"));
        assert_eq!(snapshot_path_key("src/a.ts").len(), 64);
    }

    #[test]
    fn segments_replace_foreign_characters() {
        assert_eq!(encode_segment("sess/1"), "sess_1");
        assert_eq!(encode_segment("agent:2"), "agent_2");
        assert_eq!(encode_segment("a.b-c_d"), "a.b-c_d");
    }

    #[tokio::test]
    async fn record_then_list_returns_newest_first_with_previous_id() {
        let (_dir, store) = store();
        let first = store
            .record(SnapshotRecordInput {
                at: "2026-01-01T00:00:00.000Z".into(),
                ..input("src/a.ts", Some(b"one"))
            })
            .await
            .unwrap();
        let second = store
            .record(SnapshotRecordInput {
                at: "2026-01-02T00:00:00.000Z".into(),
                ..input("src/a.ts", Some(b"two"))
            })
            .await
            .unwrap();
        assert_eq!(first.hash, snapshot_content_hash(b"one"));
        assert_eq!(second.hash, snapshot_content_hash(b"two"));
        assert_eq!(first.previous_id, None);
        assert_eq!(second.previous_id, Some(first.id.clone()));
        let listed = store.list("src/a.ts".into()).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| record.id.clone())
                .collect::<Vec<_>>(),
            vec![second.id.clone(), first.id.clone()]
        );
        assert_eq!(
            store.list("src/missing.ts".into()).await.unwrap(),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn content_returns_bytes_only_for_a_matching_hash() {
        let (_dir, store) = store();
        let record = store.record(input("src/a.ts", Some(b"body"))).await.unwrap();
        assert_eq!(
            store.content(record.hash.clone()).await.unwrap(),
            Some(b"body".to_vec())
        );
        assert!(store.content(String::new()).await.unwrap().is_none());
        assert!(store.content("0".repeat(64)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn complete_attaches_the_after_hash_and_size() {
        let (_dir, store) = store();
        let record = store.record(input("src/a.ts", Some(b"one"))).await.unwrap();
        let after_hash = snapshot_content_hash(b"two");
        assert!(store
            .complete(
                "src/a.ts".into(),
                record.id.clone(),
                after_hash.clone(),
                3,
            )
            .await
            .unwrap());
        let listed = store.list("src/a.ts".into()).await.unwrap();
        assert_eq!(listed[0].after_hash.as_deref(), Some(after_hash.as_str()));
        assert_eq!(listed[0].after_size, Some(3));
        assert!(!store
            .complete("src/a.ts".into(), "missing".into(), String::new(), 0)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn remove_deletes_a_record_once() {
        let (_dir, store) = store();
        let record = store.record(input("src/a.ts", None)).await.unwrap();
        assert!(store
            .remove("src/a.ts".into(), record.id.clone())
            .await
            .unwrap());
        assert!(!store
            .remove("src/a.ts".into(), record.id)
            .await
            .unwrap());
        assert!(store.list("src/a.ts".into()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn agent_marks_are_scoped_by_session_and_sorted_by_time() {
        let (_dir, store) = store();
        store
            .mark_agent(SnapshotAgentMarkInput {
                agent_id: "a2".into(),
                session_id: "sess-1".into(),
                at: "2026-01-02T00:00:00.000Z".into(),
                cwd: "/w".into(),
                parent_agent_id: None,
                profile_name: None,
                swarm_index: None,
            })
            .await
            .unwrap();
        store
            .mark_agent(SnapshotAgentMarkInput {
                agent_id: "a1".into(),
                session_id: "sess-1".into(),
                at: "2026-01-01T00:00:00.000Z".into(),
                cwd: "/w".into(),
                parent_agent_id: None,
                profile_name: None,
                swarm_index: None,
            })
            .await
            .unwrap();
        let marks = store.agents("sess-1".into()).await.unwrap();
        assert_eq!(
            marks.iter()
                .map(|mark| mark.agent_id.clone())
                .collect::<Vec<_>>(),
            vec!["a1".to_string(), "a2".to_string()]
        );
        assert!(store.agents("sess-2".into()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_caps_versions_per_path_and_sweeps_unreferenced_objects() {
        let (_dir, store) = store();
        for index in 0..5 {
            store
                .record(SnapshotRecordInput {
                    at: format!("2026-01-0{index}T00:00:00.000Z"),
                    size: 6,
                    bytes: Some(format!("body-{index}").into_bytes()),
                    ..input("src/a.ts", None)
                })
                .await
                .unwrap();
        }
        let result = store
            .prune(SnapshotPruneOptions {
                now_ms: 1_767_225_600_000,
                max_per_path: 2,
                max_bytes_per_path: 1024,
                max_age_ms: 365 * 24 * 60 * 60 * 1000,
                min_object_age_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(result.records_removed, 3);
        let listed = store.list("src/a.ts".into()).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].size, 6, "the newest two survive");
        assert_eq!(result.objects_removed, 3);
        assert_eq!(result.agent_marks_removed, 0);
    }
}
