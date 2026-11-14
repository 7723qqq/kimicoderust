use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::app::environment::environment::IEnvironmentService;
use crate::app::workspace::workspace::Workspace;
use crate::program::program::{Program, ProgramSnapshot};
use crate::program::program_dependencies::ProgramDependencies;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::{
    IWorkspaceContext, WorkspaceMeta, WorkspaceSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceInstanceLifecycle {
    Materializing,
    Active,
    Closing,
    Disposed,
}

#[derive(Clone)]
pub struct WorkspaceInstanceSnapshot {
    pub metadata: Workspace,
    pub lifecycle: WorkspaceInstanceLifecycle,
    pub program: ProgramSnapshot,
}

pub struct WorkspaceInstance {
    pub program: Arc<Program>,
    lifecycle: Mutex<WorkspaceInstanceLifecycle>,
    metadata: Arc<Mutex<Workspace>>,
}

impl WorkspaceInstance {
    pub fn new(
        metadata: Workspace,
        environments: Arc<dyn IEnvironmentService>,
        context: Arc<dyn IWorkspaceContext>,
        dependencies: ProgramDependencies,
    ) -> Self {
        let metadata = Arc::new(Mutex::new(metadata));
        let live_context = Arc::new(LiveWorkspaceContext {
            base: context,
            metadata: metadata.clone(),
        });
        let id = metadata.lock().id.clone();
        Self {
            program: Arc::new(Program::new(id, environments, live_context, dependencies)),
            lifecycle: Mutex::new(WorkspaceInstanceLifecycle::Materializing),
            metadata,
        }
    }

    pub fn metadata(&self) -> Workspace {
        self.metadata.lock().clone()
    }

    pub fn update(&self, metadata: Workspace) {
        *self.metadata.lock() = metadata;
    }

    pub fn id(&self) -> String {
        self.metadata.lock().id.clone()
    }

    pub fn root(&self) -> String {
        self.metadata.lock().root.clone()
    }

    pub fn activate(&self) {
        let mut lifecycle = self.lifecycle.lock();
        if *lifecycle == WorkspaceInstanceLifecycle::Materializing {
            *lifecycle = WorkspaceInstanceLifecycle::Active;
        }
    }

    pub fn snapshot(&self) -> WorkspaceInstanceSnapshot {
        WorkspaceInstanceSnapshot {
            metadata: self.metadata(),
            lifecycle: *self.lifecycle.lock(),
            program: self.program.snapshot(),
        }
    }

    pub async fn dispose(&self) -> Result<(), WireAsyncError> {
        if *self.lifecycle.lock() == WorkspaceInstanceLifecycle::Disposed {
            return Ok(());
        }
        *self.lifecycle.lock() = WorkspaceInstanceLifecycle::Closing;
        let result = self.program.dispose().await;
        *self.lifecycle.lock() = WorkspaceInstanceLifecycle::Disposed;
        result
    }
}

struct LiveWorkspaceContext {
    base: Arc<dyn IWorkspaceContext>,
    metadata: Arc<Mutex<Workspace>>,
}

impl IWorkspaceContext for LiveWorkspaceContext {
    fn workspace_id(&self) -> String {
        self.base.workspace_id()
    }

    fn cwd(&self) -> String {
        self.base.cwd()
    }

    fn source(&self) -> WorkspaceSource {
        self.base.source()
    }

    fn remote_cwd(&self) -> Option<String> {
        self.base.remote_cwd()
    }

    fn meta(&self) -> WorkspaceMeta {
        let metadata = self.metadata.lock().clone();
        WorkspaceMeta {
            id: metadata.id,
            root: metadata.root,
            name: metadata.name,
            kind: metadata.kind,
            created_at: metadata.created_at,
            last_opened_at: metadata.last_opened_at,
        }
    }

    fn persistence_scope(&self) -> String {
        self.base.persistence_scope()
    }
}

