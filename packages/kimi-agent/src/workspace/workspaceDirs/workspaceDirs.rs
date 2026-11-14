use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::_base::di::lifecycle::IDisposable;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAddDirInput {
    pub path: String,
    pub persist: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAdditionalDirsResult {
    pub project_root: String,
    pub config_path: String,
    pub additional_dirs: Vec<String>,
    pub persisted: bool,
}

pub trait IWorkspaceDirs: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn primary(&self) -> String;
    fn additional_dirs(&self) -> Vec<String>;
    fn shared_additional_dirs(&self) -> Vec<String>;
    fn primary_by_workspace(&self) -> HashMap<String, String>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn IDisposable>;
    fn add_dir(
        &self,
        input: WorkspaceAddDirInput,
    ) -> BoxFuture<'static, Result<WorkspaceAdditionalDirsResult, WireAsyncError>>;
    fn merge_additional_dirs(
        &self,
        base_dir: &str,
        dirs: &[String],
    ) -> BoxFuture<'static, Result<Box<dyn IDisposable>, WireAsyncError>>;
    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn session_info(&self) -> Arc<dyn ISessionWorkspaceInfo>;
}
