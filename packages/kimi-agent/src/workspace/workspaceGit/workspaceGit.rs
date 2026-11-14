use std::collections::HashSet;

use async_trait::async_trait;

use crate::app::git::git::{
    FsDiffResponse, FsGitStatusResponse, FsPullRequest, GitStatusOptions,
};
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait IWorkspaceGitService: Send + Sync {
    async fn status(
        &self,
        path_filter: Option<HashSet<String>>,
    ) -> Result<FsGitStatusResponse, WireAsyncError>;
    async fn status_at(
        &self,
        cwd: &str,
        options: Option<GitStatusOptions>,
    ) -> Result<FsGitStatusResponse, WireAsyncError>;
    async fn pull_request_at(&self, cwd: &str) -> Result<Option<FsPullRequest>, WireAsyncError>;
    async fn diff(&self, rel_path: &str, abs_path: &str) -> Result<FsDiffResponse, WireAsyncError>;
    async fn diff_at(
        &self,
        cwd: &str,
        rel_path: &str,
        abs_path: &str,
    ) -> Result<FsDiffResponse, WireAsyncError>;
}
