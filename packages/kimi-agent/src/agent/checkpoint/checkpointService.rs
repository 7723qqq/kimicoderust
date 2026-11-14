use std::ops::Deref;
use std::sync::{Arc, LazyLock, Weak};

use futures_util::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::event::Subscription;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::conversation_undo_participants::{
    AgentConversationUndoParticipant, IAgentConversationUndoParticipantRegistry,
};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::IAgentToolExecutorService;
use crate::turn_loop::types::{FileOperation, ToolResourceAccess};
use crate::agent::tool_executor::tool_hooks::{
    ToolDidExecuteContext, ToolExecutionOutcome,
};
use crate::app::event::event2::{Event2, Event2Class};
use crate::app::event::event_bus::IEventBus;
use crate::app::flag::flag::IFlagService;
use crate::features::operationLog::flag::OPERATION_LOG_FLAG_ID;
use crate::hooks::{HookRegisterOptions, HookRegistration};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::is_host_fs_not_found;
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageWriteOptions,
};
use crate::wire::record::WireAsyncError;

pub const CHECKPOINT_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const CHECKPOINT_MAX_TURNS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointConflict {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRestoredPayload {
    pub restored: Vec<String>,
    pub conflicts: Vec<CheckpointConflict>,
}

pub static CHECKPOINT_RESTORED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "checkpoint.restored".to_string(),
        durable: false,
        observable: true,
        agent_domain: false,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointRestored {
    event: Event2,
    pub payload: CheckpointRestoredPayload,
}

impl CheckpointRestored {
    pub const TYPE: &'static str = "checkpoint.restored";

    pub fn new(payload: CheckpointRestoredPayload) -> Self {
        let event = Event2::new(
            &CHECKPOINT_RESTORED_CLASS,
            serde_json::to_value(&payload).expect("checkpoint restored event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        CHECKPOINT_RESTORED_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for CheckpointRestored {
    type Target = CheckpointRestoredPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<CheckpointRestored> for Event2 {
    fn from(event: CheckpointRestored) -> Self {
        event.event
    }
}

pub trait IAgentCheckpointService: Send + Sync {}

#[derive(Debug, Clone)]
struct FileSnapshot {
    path: String,
    existed: Option<bool>,
    preimage_sha: String,
    blob_key: String,
    after_sha: Option<String>,
}

#[derive(Debug, Clone)]
struct CheckpointGroup {
    turn_id: u64,
    context_len: usize,
    files: IndexMap<String, FileSnapshot>,
    gaps: IndexSet<String>,
}

#[derive(Default)]
struct CheckpointRegistrations {
    will_execute: Option<Subscription>,
    after_execute: Option<HookRegistration<ToolDidExecuteContext>>,
    undo: Option<DisposableRef>,
}

pub struct AgentCheckpointService {
    weak_self: Weak<Self>,
    checkpoints: Mutex<IndexMap<u64, CheckpointGroup>>,
    storage_scope: String,
    context: Arc<dyn IAgentContextMemoryService>,
    storage: Arc<dyn IFileSystemStorageService>,
    fs: Arc<dyn IHostFileSystem>,
    event_bus: Arc<dyn IEventBus>,
    _registrations: Mutex<CheckpointRegistrations>,
}

impl AgentCheckpointService {
    pub fn new(
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        context: Arc<dyn IAgentContextMemoryService>,
        undo_registry: Arc<dyn IAgentConversationUndoParticipantRegistry>,
        agent: Arc<dyn IAgentScopeContext>,
        storage: Arc<dyn IFileSystemStorageService>,
        fs: Arc<dyn IHostFileSystem>,
        event_bus: Arc<dyn IEventBus>,
        flags: Arc<dyn IFlagService>,
    ) -> Arc<Self> {
        let storage_scope = agent.scope(Some("checkpoints"));
        let this = Arc::new_cyclic(|weak| Self {
            weak_self: weak.clone(),
            checkpoints: Mutex::new(IndexMap::new()),
            storage_scope,
            context,
            storage,
            fs,
            event_bus,
            _registrations: Mutex::new(CheckpointRegistrations::default()),
        });
        if flags.enabled(OPERATION_LOG_FLAG_ID) {
            return this;
        }
        let weak = Arc::downgrade(&this);
        let will_execute = tool_executor.on_will_execute_tool().subscribe(move |event| {
            let write_paths = collect_write_paths(event.execution.accesses.as_deref());
            if write_paths.is_empty() {
                return;
            }
            let Some(this) = weak.upgrade() else {
                return;
            };
            let turn_id = event.turn_id;
            let _ = event.wait_until(Box::pin(async move {
                this.capture_paths(turn_id, &write_paths)
                    .await
                    .map_err(|error| error.to_string())
            }));
        });
        let weak = Arc::downgrade(&this);
        let after_execute = tool_executor
            .hooks()
            .on_did_execute_tool
            .register(
                "checkpoint-after",
                Arc::new(move |context, next| {
                    let weak = weak.clone();
                    Box::pin(async move {
                        if let Some(this) = weak.upgrade() {
                            this.record_after_write(&context).await;
                        }
                        next.proceed(Some(context)).await;
                    })
                }),
                HookRegisterOptions::default(),
            )
            .expect("checkpoint after-write hook registration is valid");
        let participant: Arc<dyn AgentConversationUndoParticipant> = this.clone();
        let undo = undo_registry
            .register(participant)
            .expect("checkpoint undo participant registration is unique");
        let mut registrations = this._registrations.lock();
        registrations.will_execute = Some(will_execute);
        registrations.after_execute = Some(after_execute);
        registrations.undo = Some(undo);
        drop(registrations);
        this
    }

    fn ensure_group(&self, turn_id: u64) {
        let mut checkpoints = self.checkpoints.lock();
        if checkpoints.contains_key(&turn_id) {
            return;
        }
        checkpoints.insert(
            turn_id,
            CheckpointGroup {
                turn_id,
                context_len: self.context.get().len(),
                files: IndexMap::new(),
                gaps: IndexSet::new(),
            },
        );
        if checkpoints.len() > CHECKPOINT_MAX_TURNS {
            if let Some(oldest) = checkpoints.keys().copied().min() {
                checkpoints.shift_remove(&oldest);
            }
        }
    }

    async fn capture_paths(
        &self,
        turn_id: u64,
        paths: &[String],
    ) -> Result<(), WireAsyncError> {
        self.ensure_group(turn_id);
        for path in paths {
            if self
                .checkpoints
                .lock()
                .get(&turn_id)
                .is_some_and(|group| group.files.contains_key(path))
            {
                continue;
            }
            let snapshot = self.capture_file(path).await?;
            let mut checkpoints = self.checkpoints.lock();
            let Some(group) = checkpoints.get_mut(&turn_id) else {
                continue;
            };
            match snapshot {
                Some(snapshot) => {
                    group.files.insert(path.clone(), snapshot);
                }
                None => {
                    group.gaps.insert("unreadable".to_string());
                }
            }
        }
        Ok(())
    }

    async fn capture_file(&self, path: &str) -> Result<Option<FileSnapshot>, WireAsyncError> {
        let mut existed = Some(true);
        let mut bytes = None;
        match self.fs.stat(path).await {
            Ok(stat) => {
                if stat.size > CHECKPOINT_MAX_FILE_BYTES {
                    return Ok(None);
                }
                match self
                    .fs
                    .read_bytes(path, CHECKPOINT_MAX_FILE_BYTES + 1, None)
                    .await
                {
                    Ok(content) => {
                        if content.len() as u64 > CHECKPOINT_MAX_FILE_BYTES {
                            return Ok(None);
                        }
                        bytes = Some(content);
                    }
                    Err(error) => {
                        existed = if is_host_fs_not_found(&error) {
                            Some(false)
                        } else {
                            None
                        };
                    }
                }
            }
            Err(error) => {
                existed = if is_host_fs_not_found(&error) {
                    Some(false)
                } else {
                    None
                };
            }
        }
        let path_sha = sha256_bytes(path.as_bytes());
        let key = format!("{}-{}.bin", &path_sha[..16], uuid::Uuid::new_v4());
        if let Some(content) = bytes.as_ref().filter(|content| !content.is_empty()) {
            self.storage
                .write(
                    &self.storage_scope,
                    &key,
                    content.clone(),
                    Some(StorageWriteOptions {
                        atomic: Some(true),
                        signal: None,
                    }),
                )
                .await?;
        }
        Ok(Some(FileSnapshot {
            path: path.to_string(),
            existed,
            preimage_sha: bytes.as_deref().map_or_else(String::new, sha256_bytes),
            blob_key: key,
            after_sha: None,
        }))
    }

    async fn record_after_write(&self, context: &ToolDidExecuteContext) {
        if context.outcome != ToolExecutionOutcome::Executed {
            return;
        }
        let write_paths = collect_write_paths(context.accesses.as_deref());
        if write_paths.is_empty() {
            return;
        }
        let turn_id = context.base.turn_id;
        if !self.checkpoints.lock().contains_key(&turn_id) {
            return;
        }
        for path in write_paths {
            if !self
                .checkpoints
                .lock()
                .get(&turn_id)
                .is_some_and(|group| group.files.contains_key(&path))
            {
                continue;
            }
            match self
                .fs
                .read_bytes(&path, CHECKPOINT_MAX_FILE_BYTES + 1, None)
                .await
            {
                Ok(bytes) => {
                    if bytes.len() as u64 <= CHECKPOINT_MAX_FILE_BYTES {
                        let after_sha = Some(sha256_bytes(&bytes));
                        if let Some(snapshot) = self
                            .checkpoints
                            .lock()
                            .get_mut(&turn_id)
                            .and_then(|group| group.files.get_mut(&path))
                        {
                            snapshot.after_sha = after_sha;
                        }
                    }
                }
                Err(_) => {
                    if let Some(snapshot) = self
                        .checkpoints
                        .lock()
                        .get_mut(&turn_id)
                        .and_then(|group| group.files.get_mut(&path))
                    {
                        snapshot.after_sha = None;
                    }
                }
            }
        }
    }

    async fn restore_after_undo(&self) -> Result<(), WireAsyncError> {
        let current_len = self.context.get().len();
        let turn_ids = self
            .checkpoints
            .lock()
            .iter()
            .filter(|(_, group)| group.context_len > current_len)
            .map(|(turn_id, _)| *turn_id)
            .collect::<Vec<_>>();
        let mut restored = Vec::new();
        let mut conflicts = Vec::new();
        for turn_id in turn_ids {
            let group = self.checkpoints.lock().get(&turn_id).cloned();
            let Some(group) = group else {
                continue;
            };
            for (path, snapshot) in &group.files {
                let outcome = self.restore_file(path, snapshot).await?;
                match outcome {
                    "restored" => restored.push(path.clone()),
                    "skipped" => {}
                    reason => conflicts.push(CheckpointConflict {
                        path: path.clone(),
                        reason: reason.to_string(),
                    }),
                }
            }
            self.checkpoints.lock().shift_remove(&turn_id);
        }
        if !restored.is_empty() || !conflicts.is_empty() {
            self.event_bus.publish(
                CheckpointRestored::new(CheckpointRestoredPayload {
                    restored,
                    conflicts,
                })
                .into(),
                None,
            );
        }
        Ok(())
    }

    async fn restore_file(
        &self,
        path: &str,
        snapshot: &FileSnapshot,
    ) -> Result<&'static str, WireAsyncError> {
        match self
            .fs
            .read_bytes(path, CHECKPOINT_MAX_FILE_BYTES + 1, None)
            .await
        {
            Ok(current) => {
                if snapshot
                    .after_sha
                    .as_deref()
                    .is_some_and(|after_sha| sha256_bytes(&current) != after_sha)
                {
                    return Ok("manual_edit");
                }
            }
            Err(_) => {
                if snapshot.existed != Some(true) {
                    return Ok("skipped");
                }
                return Ok("deleted");
            }
        }
        if snapshot.existed != Some(true) {
            if snapshot.existed.is_none() || snapshot.after_sha.is_none() {
                return Ok("unknown_preimage");
            }
            let _ = self.fs.remove(path).await;
            return Ok("restored");
        }
        let bytes = self
            .storage
            .read(&self.storage_scope, &snapshot.blob_key)
            .await
            .ok()
            .flatten();
        let Some(bytes) = bytes else {
            return Ok("missing_payload");
        };
        self.fs.write_bytes(path, &bytes).await?;
        Ok("restored")
    }
}

impl IAgentCheckpointService for AgentCheckpointService {}

impl Drop for AgentCheckpointService {
    fn drop(&mut self) {
        let registrations = self._registrations.get_mut();
        if let Some(subscription) = registrations.will_execute.take() {
            subscription.dispose();
        }
        if let Some(registration) = registrations.after_execute.take() {
            registration.dispose();
        }
        if let Some(registration) = registrations.undo.take() {
            registration.dispose();
        }
    }
}

impl AgentConversationUndoParticipant for AgentCheckpointService {
    fn id(&self) -> &str {
        "checkpoint"
    }

    fn phase(&self) -> Option<&'static str> {
        None
    }

    fn reconcile_after_undo(&self) -> BoxFuture<'static, Result<(), String>> {
        let this = self.weak_self.clone();
        Box::pin(async move {
            let Some(this) = this.upgrade() else {
                return Ok(());
            };
            this.restore_after_undo()
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_write_paths(accesses: Option<&[ToolResourceAccess]>) -> Vec<String> {
    let mut paths = Vec::new();
    for access in accesses.unwrap_or_default() {
        let ToolResourceAccess::File(access) = access else {
            continue;
        };
        if matches!(access.operation, FileOperation::Write | FileOperation::ReadWrite) {
            paths.push(access.path.clone());
        }
    }
    paths
}
