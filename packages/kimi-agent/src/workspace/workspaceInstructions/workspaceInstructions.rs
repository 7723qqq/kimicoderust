use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::human::utils::watch::WatchChange;
use crate::session::session_instructions::instructions_provider::ISessionInstructionsProvider;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInstructionsSnapshot {
    pub agents_md: Option<String>,
    pub agents_md_warning: Option<String>,
    pub agents_md_paths: Option<Vec<String>>,
}

pub trait IWorkspaceInstructionsService: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn snapshot(&self) -> WorkspaceInstructionsSnapshot;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(Vec<WatchChange>) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn session_provider(&self) -> Arc<dyn ISessionInstructionsProvider>;
}
