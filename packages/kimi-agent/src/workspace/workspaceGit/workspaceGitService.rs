use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::instantiation::LiveRef;
use crate::app::git::git::{
    FsDiffResponse, FsGitStatusResponse, FsPullRequest, GitStatusOptions, IGitService,
};
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::workspace_git::IWorkspaceGitService;

pub struct WorkspaceGitService {
    dirs: Arc<dyn IWorkspaceDirs>,
    git: LiveRef<Arc<dyn IGitService>>,
}

impl WorkspaceGitService {
    pub fn new(dirs: Arc<dyn IWorkspaceDirs>, git: LiveRef<Arc<dyn IGitService>>) -> Self {
        Self { dirs, git }
    }
}

#[async_trait]
impl IWorkspaceGitService for WorkspaceGitService {
    async fn status(
        &self,
        path_filter: Option<HashSet<String>>,
    ) -> Result<FsGitStatusResponse, WireAsyncError> {
        self.git
            .current()
            .unwrap()
            .status(
                &self.dirs.primary(),
                Some(GitStatusOptions {
                    path_filter,
                    ..Default::default()
                }),
            )
            .await
    }

    async fn status_at(
        &self,
        cwd: &str,
        options: Option<GitStatusOptions>,
    ) -> Result<FsGitStatusResponse, WireAsyncError> {
        self.git.current().unwrap().status(cwd, options).await
    }

    async fn pull_request_at(&self, cwd: &str) -> Result<Option<FsPullRequest>, WireAsyncError> {
        self.git.current().unwrap().pull_request(cwd).await
    }

    async fn diff(&self, rel_path: &str, abs_path: &str) -> Result<FsDiffResponse, WireAsyncError> {
        self.git
            .current()
            .unwrap()
            .diff(&self.dirs.primary(), rel_path, abs_path)
            .await
    }

    async fn diff_at(
        &self,
        cwd: &str,
        rel_path: &str,
        abs_path: &str,
    ) -> Result<FsDiffResponse, WireAsyncError> {
        self.git
            .current()
            .unwrap()
            .diff(cwd, rel_path, abs_path)
            .await
    }
}
