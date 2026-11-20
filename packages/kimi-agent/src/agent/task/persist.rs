use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock};

use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::errors::errors::BugIndicatingError;
use crate::environment::environment::EnvironmentPath;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageReadRange,
};
use crate::wire::record::WireAsyncError;

use super::text_align::align_to_character_start;
use super::types::AgentTaskInfo;

static VALID_TASK_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*-[0-9a-z]{8}$").expect("valid task id expression")
});

const TASK_READ_CONCURRENCY: usize = 16;
const TASKS_SCOPE: &str = "tasks";
const OUTPUT_LOG_KEY: &str = "output.log";
const JSON_SUFFIX: &str = ".json";
const OUTPUT_TAIL_ALIGN_BYTES: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskPersistenceRoot {
    pub dir: String,
    pub scope: String,
}

#[derive(Clone)]
pub struct AgentTaskSpillTarget {
    pub fs: Arc<dyn IHostFileSystem>,
    pub dir: String,
    pub path: Option<EnvironmentPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskStoredOutputSnapshot {
    pub output_path: String,
    pub output_size_bytes: u64,
    pub preview_bytes: u64,
    pub truncated: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskOutputTail {
    pub offset: u64,
    pub data: Vec<u8>,
}

#[derive(Clone)]
struct TaskOutputData {
    root: AgentTaskPersistenceRoot,
    data: Vec<u8>,
}

#[derive(Clone)]
struct TaskOutputLocation {
    root: AgentTaskPersistenceRoot,
    total_bytes: u64,
}

#[derive(Clone)]
struct MirrorRecord {
    scope: String,
    size: u64,
}

fn validate_task_id(task_id: &str) -> Result<(), WireAsyncError> {
    if !VALID_TASK_ID.is_match(task_id) {
        return Err(Box::new(BugIndicatingError::new(format!(
            "Invalid task id: \"{task_id}\""
        ))));
    }
    Ok(())
}

fn valid_task_id(task_id: &str) -> bool {
    VALID_TASK_ID.is_match(task_id)
}

fn join_path(base: &str, parts: &[&str]) -> String {
    let mut output = base.to_string();
    for part in parts {
        if !output.is_empty() && !output.ends_with(['/', '\\']) {
            output.push('/');
        }
        output.push_str(part.trim_matches(['/', '\\']));
    }
    output
}

fn spill_path(target: &AgentTaskSpillTarget, task_id: &str) -> String {
    let name = format!("{task_id}.log");
    match &target.path {
        Some(path) => path.join(&target.dir, &[&name]),
        None => join_path(&target.dir, &[&name]),
    }
}

pub fn is_task_output_file_path(path: &str, agent_dir: &str, path_api: &EnvironmentPath) -> bool {
    if !path_api.is_absolute(path) || path.contains('\0') {
        return false;
    }
    let relative = path_api.relative(agent_dir, path).replace('\\', "/");
    let parts = relative.split('/').collect::<Vec<_>>();
    parts.len() == 3
        && parts[0] == TASKS_SCOPE
        && valid_task_id(parts[1])
        && parts[2] == OUTPUT_LOG_KEY
}

pub struct AgentTaskPersistence {
    agent_dir: String,
    agent_scope: String,
    docs: Arc<dyn IAtomicDocumentStore>,
    bytes: Arc<dyn IFileSystemStorageService>,
    fallback_root: Option<AgentTaskPersistenceRoot>,
    spill_target: Option<Arc<dyn Fn() -> Option<AgentTaskSpillTarget> + Send + Sync>>,
    mirrors: Mutex<HashMap<usize, HashMap<String, MirrorRecord>>>,
    output_sizes: Mutex<HashMap<String, u64>>,
    output_tails: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl AgentTaskPersistence {
    pub fn new(
        agent_dir: impl Into<String>,
        agent_scope: impl Into<String>,
        docs: Arc<dyn IAtomicDocumentStore>,
        bytes: Arc<dyn IFileSystemStorageService>,
        fallback_root: Option<AgentTaskPersistenceRoot>,
        spill_target: Option<Arc<dyn Fn() -> Option<AgentTaskSpillTarget> + Send + Sync>>,
    ) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            agent_scope: agent_scope.into(),
            docs,
            bytes,
            fallback_root,
            spill_target,
            mirrors: Mutex::new(HashMap::new()),
            output_sizes: Mutex::new(HashMap::new()),
            output_tails: Mutex::new(HashMap::new()),
        }
    }

    fn primary_root(&self) -> AgentTaskPersistenceRoot {
        AgentTaskPersistenceRoot {
            dir: self.agent_dir.clone(),
            scope: self.agent_scope.clone(),
        }
    }

    fn tasks_scope(&self, root: &AgentTaskPersistenceRoot) -> String {
        format!("{}/{TASKS_SCOPE}", root.scope)
    }

    fn task_output_scope(
        &self,
        task_id: &str,
        root: &AgentTaskPersistenceRoot,
    ) -> Result<String, WireAsyncError> {
        validate_task_id(task_id)?;
        Ok(format!("{}/{TASKS_SCOPE}/{task_id}", root.scope))
    }

    fn task_output_file_at(
        &self,
        task_id: &str,
        root: &AgentTaskPersistenceRoot,
    ) -> Result<String, WireAsyncError> {
        validate_task_id(task_id)?;
        Ok(join_path(&root.dir, &[TASKS_SCOPE, task_id, OUTPUT_LOG_KEY]))
    }

    async fn output_path_for(
        &self,
        task_id: &str,
        output: &TaskOutputData,
    ) -> Result<String, WireAsyncError> {
        if let Some(target) = self.spill_target.as_ref().and_then(|target| target())
            && self.mirror_output(task_id, output, &target).await
        {
            return Ok(spill_path(&target, task_id));
        }
        self.task_output_file_at(task_id, &output.root)
    }

    async fn mirror_output(
        &self,
        task_id: &str,
        output: &TaskOutputData,
        target: &AgentTaskSpillTarget,
    ) -> bool {
        let fs_key = Arc::as_ptr(&target.fs) as *const () as usize;
        let path = spill_path(target, task_id);
        if self
            .mirrors
            .lock()
            .get(&fs_key)
            .and_then(|mirrors| mirrors.get(&path))
            .is_some_and(|mirror| mirror.scope == output.root.scope && mirror.size == output.data.len() as u64)
        {
            return true;
        }
        let result = async {
            target.fs.mkdir(&target.dir, true).await?;
            target
                .fs
                .write_text(&path, &String::from_utf8_lossy(&output.data))
                .await
        }
        .await;
        if result.is_ok() {
            self.mirrors.lock().entry(fs_key).or_default().insert(
                path,
                MirrorRecord {
                    scope: output.root.scope.clone(),
                    size: output.data.len() as u64,
                },
            );
            true
        } else {
            if let Some(mirrors) = self.mirrors.lock().get_mut(&fs_key) {
                mirrors.remove(&path);
            }
            false
        }
    }

    async fn serialize_output<T, F>(&self, task_id: &str, operation: F) -> Result<T, WireAsyncError>
    where
        F: Future<Output = Result<T, WireAsyncError>>,
    {
        validate_task_id(task_id)?;
        let tail = self
            .output_tails
            .lock()
            .entry(task_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let guard = tail.lock().await;
        let result = operation.await;
        drop(guard);
        if Arc::strong_count(&tail) == 2 {
            self.output_tails.lock().remove(task_id);
        }
        result
    }

    pub fn task_output_file(&self, task_id: &str) -> Result<String, WireAsyncError> {
        self.task_output_file_at(task_id, &self.primary_root())
    }

    pub async fn write_task(&self, task: &AgentTaskInfo) -> Result<(), WireAsyncError> {
        let task_id = task_id_of(task);
        validate_task_id(&task_id)?;
        self.docs
            .set(
                &self.tasks_scope(&self.primary_root()),
                &format!("{task_id}{JSON_SUFFIX}"),
                serde_json::to_value(task)?,
            )
            .await
    }

    pub async fn read_task(&self, task_id: &str) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        validate_task_id(task_id)?;
        let key = format!("{task_id}{JSON_SUFFIX}");
        if let Some(value) = self
            .docs
            .get(&self.tasks_scope(&self.primary_root()), &key)
            .await?
        {
            return Ok(normalize_persisted_task(&value));
        }
        let Some(fallback) = &self.fallback_root else {
            return Ok(None);
        };
        let value = self
            .docs
            .get(&self.tasks_scope(fallback), &key)
            .await?;
        Ok(value.as_ref().and_then(normalize_persisted_task))
    }

    pub async fn append_task_output(&self, task_id: &str, chunk: &str) -> Result<(), WireAsyncError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let task_id = task_id.to_string();
        let chunk = chunk.to_string();
        self.serialize_output(&task_id, async {
            let root = self.primary_root();
            let scope = self.task_output_scope(&task_id, &root)?;
            let cached_size = self.output_sizes.lock().get(&task_id).copied();
            let previous_size = match cached_size {
                Some(size) => size,
                None => self.bytes.size(&scope, OUTPUT_LOG_KEY).await?.unwrap_or(0),
            };
            let data = chunk.as_bytes().to_vec();
            self.bytes.append(&scope, OUTPUT_LOG_KEY, data, None).await?;
            let size = previous_size + chunk.len() as u64;
            self.output_sizes.lock().insert(task_id.clone(), size);
            let Some(target) = self.spill_target.as_ref().and_then(|target| target()) else {
                return Ok(());
            };
            let path = spill_path(&target, &task_id);
            let fs_key = Arc::as_ptr(&target.fs) as *const () as usize;
            let can_append = self
                .mirrors
                .lock()
                .get(&fs_key)
                .and_then(|mirrors| mirrors.get(&path))
                .is_some_and(|mirror| mirror.scope == self.agent_scope && mirror.size == previous_size);
            if can_append {
                if target.fs.append_text(&path, &chunk).await.is_ok() {
                    self.mirrors.lock().entry(fs_key).or_default().insert(
                        path,
                        MirrorRecord {
                            scope: self.agent_scope.clone(),
                            size,
                        },
                    );
                } else if let Some(mirrors) = self.mirrors.lock().get_mut(&fs_key) {
                    mirrors.remove(&path);
                }
                return Ok(());
            }
            if let Some(output) = self.read_task_output_data(&task_id).await? {
                self.mirror_output(&task_id, &output, &target).await;
            }
            Ok(())
        })
        .await
    }

    pub async fn task_output_size_bytes(&self, task_id: &str) -> Result<u64, WireAsyncError> {
        Ok(self
            .locate_task_output(task_id)
            .await?
            .map_or(0, |location| location.total_bytes))
    }

    pub async fn task_output_exists(&self, task_id: &str) -> Result<bool, WireAsyncError> {
        Ok(self.locate_task_output(task_id).await?.is_some())
    }

    pub async fn read_task_output_bytes(
        &self,
        task_id: &str,
        offset: i64,
        max_bytes: i64,
    ) -> Result<String, WireAsyncError> {
        let start = offset.max(0) as u64;
        let limit = max_bytes.max(0) as u64;
        if limit == 0 {
            return Ok(String::new());
        }
        let Some(output) = self.read_task_output_data(task_id).await? else {
            return Ok(String::new());
        };
        if start >= output.data.len() as u64 {
            return Ok(String::new());
        }
        let end = ((start + limit) as usize).min(output.data.len());
        Ok(String::from_utf8_lossy(&output.data[start as usize..end]).into_owned())
    }

    pub async fn read_task_output_tail_bytes(
        &self,
        task_id: &str,
        max_bytes: i64,
    ) -> Result<Option<AgentTaskOutputTail>, WireAsyncError> {
        let roots = std::iter::once(self.primary_root())
            .chain(self.fallback_root.clone())
            .collect::<Vec<_>>();
        for root in roots {
            let scope = self.task_output_scope(task_id, &root)?;
            let Some(size) = self.bytes.size(&scope, OUTPUT_LOG_KEY).await? else {
                continue;
            };
            let offset = size.saturating_sub(max_bytes.max(0) as u64);
            if offset == size {
                return Ok(Some(AgentTaskOutputTail {
                    offset,
                    data: Vec::new(),
                }));
            }
            let data = self
                .read_output_range(task_id, &root, offset, size - 1)
                .await?;
            return Ok(Some(AgentTaskOutputTail { offset, data }));
        }
        Ok(None)
    }

    pub async fn read_task_output_snapshot(
        &self,
        task_id: &str,
        max_preview_bytes: i64,
    ) -> Result<Option<AgentTaskStoredOutputSnapshot>, WireAsyncError> {
        let Some(location) = self.locate_task_output(task_id).await? else {
            return Ok(None);
        };
        let preview_limit = max_preview_bytes.max(0) as u64;
        let preview_bytes = preview_limit.min(location.total_bytes);
        if preview_bytes == 0 || location.total_bytes == 0 {
            return Ok(Some(AgentTaskStoredOutputSnapshot {
                output_path: self.task_output_file_at(task_id, &location.root)?,
                output_size_bytes: location.total_bytes,
                preview_bytes,
                truncated: preview_bytes < location.total_bytes,
                preview: String::new(),
            }));
        }
        let from = location.total_bytes - preview_bytes;
        let start = from.saturating_sub(OUTPUT_TAIL_ALIGN_BYTES);
        let window = self
            .read_output_range(task_id, &location.root, start, location.total_bytes - 1)
            .await?;
        let offset = align_to_character_start(&window, (from - start) as usize);
        Ok(Some(AgentTaskStoredOutputSnapshot {
            output_path: self.task_output_file_at(task_id, &location.root)?,
            output_size_bytes: location.total_bytes,
            preview_bytes,
            truncated: preview_bytes < location.total_bytes,
            preview: String::from_utf8_lossy(&window[offset..]).into_owned(),
        }))
    }

    async fn locate_task_output(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskOutputLocation>, WireAsyncError> {
        let primary = self.primary_root();
        if let Some(size) = self
            .bytes
            .size(&self.task_output_scope(task_id, &primary)?, OUTPUT_LOG_KEY)
            .await?
        {
            return Ok(Some(TaskOutputLocation {
                root: primary,
                total_bytes: size,
            }));
        }
        let Some(fallback) = &self.fallback_root else {
            return Ok(None);
        };
        let size = self
            .bytes
            .size(&self.task_output_scope(task_id, fallback)?, OUTPUT_LOG_KEY)
            .await?;
        Ok(size.map(|size| TaskOutputLocation {
            root: fallback.clone(),
            total_bytes: size,
        }))
    }

    async fn read_task_output_tail(
        &self,
        location: &TaskOutputLocation,
        task_id: &str,
        preview_bytes: u64,
    ) -> Result<String, WireAsyncError> {
        if preview_bytes == 0 || location.total_bytes == 0 {
            return Ok(String::new());
        }
        let from = location.total_bytes - preview_bytes;
        let start = from.saturating_sub(OUTPUT_TAIL_ALIGN_BYTES);
        let window = self
            .read_output_range(task_id, &location.root, start, location.total_bytes - 1)
            .await?;
        if window.is_empty() {
            return Ok(String::new());
        }
        let offset = align_to_character_start(&window, (from - start) as usize);
        Ok(String::from_utf8_lossy(&window[offset..]).into_owned())
    }

    async fn read_output_range(
        &self,
        task_id: &str,
        root: &AgentTaskPersistenceRoot,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, WireAsyncError> {
        let scope = self.task_output_scope(task_id, root)?;
        self.bytes
            .read_stream(
                &scope,
                OUTPUT_LOG_KEY,
                Some(StorageReadRange { start, end }),
            )
            .try_fold(Vec::new(), |mut output, chunk| async move {
                output.extend(chunk);
                Ok(output)
            })
            .await
    }

    pub async fn list_tasks(&self) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        let primary = self.list_tasks_at(self.primary_root()).await?;
        let mut entries = primary.tasks;
        if let Some(fallback) = &self.fallback_root {
            let fallback = self.list_tasks_at(fallback.clone()).await?;
            entries.extend(
                fallback
                    .tasks
                    .into_iter()
                    .filter(|entry| !primary.reserved_ids.contains(&entry.key_id)),
            );
        }
        let mut tasks = entries
            .into_iter()
            .filter_map(|entry| entry.task)
            .collect::<Vec<_>>();
        tasks.sort_by_key(task_id_of);
        Ok(tasks)
    }

    async fn list_tasks_at(
        &self,
        root: AgentTaskPersistenceRoot,
    ) -> Result<ListedTasks, WireAsyncError> {
        let mut keys = self.docs.list(&self.tasks_scope(&root), None).await?;
        keys.sort();
        let mut reserved_ids = HashSet::new();
        let mut candidates = Vec::new();
        for key in keys {
            if !key.ends_with(JSON_SUFFIX) {
                continue;
            }
            let id = key[..key.len() - JSON_SUFFIX.len()].to_string();
            if !valid_task_id(&id) {
                continue;
            }
            reserved_ids.insert(id.clone());
            candidates.push((id, key));
        }
        let fetched = futures_util::stream::iter(candidates.into_iter().map(|(id, key)| {
            let root = root.clone();
            async move {
                let task = self
                    .docs
                    .get(&self.tasks_scope(&root), &key)
                    .await
                    .ok()
                    .flatten();
                ListedTask {
                    key_id: id,
                    task: task.as_ref().and_then(normalize_persisted_task),
                }
            }
        }))
        .buffered(TASK_READ_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        Ok(ListedTasks {
            reserved_ids,
            tasks: fetched
                .into_iter()
                .filter(|entry| entry.task.is_some())
                .map(|entry| ListedTask {
                    key_id: entry.key_id,
                    task: entry.task,
                })
                .collect(),
        })
    }

    async fn read_task_output_data(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskOutputData>, WireAsyncError> {
        let primary = self.primary_root();
        if let Some(data) = self
            .bytes
            .read(&self.task_output_scope(task_id, &primary)?, OUTPUT_LOG_KEY)
            .await?
        {
            return Ok(Some(TaskOutputData {
                root: primary,
                data,
            }));
        }
        let Some(fallback) = &self.fallback_root else {
            return Ok(None);
        };
        let data = self
            .bytes
            .read(&self.task_output_scope(task_id, fallback)?, OUTPUT_LOG_KEY)
            .await?;
        Ok(data.map(|data| TaskOutputData {
            root: fallback.clone(),
            data,
        }))
    }
}

struct ListedTask {
    key_id: String,
    task: Option<AgentTaskInfo>,
}

struct ListedTasks {
    reserved_ids: HashSet<String>,
    tasks: Vec<ListedTask>,
}

fn task_id_of(task: &AgentTaskInfo) -> String {
    serde_json::to_value(task)
        .ok()
        .and_then(|value| value.get("taskId")?.as_str().map(ToString::to_string))
        .unwrap_or_default()
}

fn normalize_persisted_task(value: &Value) -> Option<AgentTaskInfo> {
    if !is_readable_persisted_task(value) {
        return None;
    }
    if value.get("task_id").is_some() {
        let legacy = serde_json::from_value::<LegacyPersistedTask>(value.clone()).ok()?;
        return legacy_persisted_task_to_info(&legacy);
    }
    let mut normalized = value.as_object()?.clone();
    if normalized.get("detached").is_none_or(Value::is_null) {
        normalized.insert("detached".to_string(), Value::Bool(true));
    }
    serde_json::from_value(Value::Object(normalized)).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyAgentTaskStatus {
    Running,
    AwaitingApproval,
    Completed,
    Failed,
    Killed,
    Lost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyPersistedTask {
    task_id: String,
    command: String,
    description: String,
    pid: u32,
    started_at: f64,
    ended_at: Option<f64>,
    exit_code: Option<i32>,
    status: LegacyAgentTaskStatus,
    #[serde(default)]
    timed_out: Option<bool>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    timeout_ms: Option<f64>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    subagent_type: Option<String>,
}

fn optional_non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn legacy_persisted_task_to_info(task: &LegacyPersistedTask) -> Option<AgentTaskInfo> {
    let status = match task.status {
        LegacyAgentTaskStatus::AwaitingApproval => "running",
        LegacyAgentTaskStatus::Failed if task.timed_out == Some(true) => "timed_out",
        LegacyAgentTaskStatus::Running => "running",
        LegacyAgentTaskStatus::Completed => "completed",
        LegacyAgentTaskStatus::Failed => "failed",
        LegacyAgentTaskStatus::Killed => "killed",
        LegacyAgentTaskStatus::Lost => "lost",
    };
    let mut value = Map::from_iter([
        ("taskId".to_string(), Value::String(task.task_id.clone())),
        ("description".to_string(), Value::String(task.description.clone())),
        ("status".to_string(), Value::String(status.to_string())),
        ("detached".to_string(), Value::Bool(true)),
        ("startedAt".to_string(), json!(task.started_at)),
        ("endedAt".to_string(), json!(task.ended_at)),
    ]);
    if let Some(stop_reason) = optional_non_empty_string(task.stop_reason.as_deref()) {
        value.insert("stopReason".to_string(), Value::String(stop_reason));
    }
    if let Some(timeout_ms) = task.timeout_ms {
        value.insert("timeoutMs".to_string(), json!(timeout_ms));
    }
    if task.task_id.starts_with("agent-") {
        value.insert("kind".to_string(), Value::String("agent".to_string()));
        if let Some(agent_id) = optional_non_empty_string(task.agent_id.as_deref()) {
            value.insert("agentId".to_string(), Value::String(agent_id));
        }
        if let Some(subagent_type) = optional_non_empty_string(task.subagent_type.as_deref()) {
            value.insert("subagentType".to_string(), Value::String(subagent_type));
        }
    } else {
        value.insert("kind".to_string(), Value::String("process".to_string()));
        value.insert("command".to_string(), Value::String(task.command.clone()));
        value.insert("pid".to_string(), Value::from(task.pid));
        value.insert("exitCode".to_string(), json!(task.exit_code));
    }
    serde_json::from_value(Value::Object(value)).ok()
}

fn is_readable_persisted_task(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.get("taskId").is_some_and(Value::is_string)
            || object.get("task_id").is_some_and(Value::is_string)
    })
}
