use std::sync::Arc;

use crate::workspace::workspace_fs::fs::IWorkspaceFsService;

pub trait ISessionWorkdir: Send + Sync {
    fn environment_id(&self) -> String;
    fn cwd(&self) -> Option<String>;
    fn fs(&self) -> Arc<dyn IWorkspaceFsService>;
}

pub struct SessionWorkdir {
    pub environment_id: String,
    pub cwd: Option<String>,
    pub fs: Arc<dyn IWorkspaceFsService>,
}

impl ISessionWorkdir for SessionWorkdir {
    fn environment_id(&self) -> String {
        self.environment_id.clone()
    }

    fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    fn fs(&self) -> Arc<dyn IWorkspaceFsService> {
        self.fs.clone()
    }
}
