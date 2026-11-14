use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::app::session_index::session_index::{ISessionIndex, SessionCountQuery, SessionListQuery, SessionSummary};
use crate::app::workspace_aliases::workspace_aliases::IWorkspaceAliases;
use crate::wire::record::WireAsyncError;

use super::workspace_sessions::{IWorkspaceSessions, RECENT_SESSIONS_LIMIT};

pub struct WorkspaceSessionsService {
    aliases: Arc<dyn IWorkspaceAliases>,
    index: Arc<dyn ISessionIndex>,
}

impl WorkspaceSessionsService {
    pub fn new(aliases: Arc<dyn IWorkspaceAliases>, index: Arc<dyn ISessionIndex>) -> Self {
        Self { aliases, index }
    }
}

#[async_trait]
impl IWorkspaceSessions for WorkspaceSessionsService {
    async fn list_recent(&self, workspace_id: &str) -> Result<Vec<SessionSummary>, WireAsyncError> {
        let workspace_ids = self.aliases.resolve_alias_ids(workspace_id).await?;
        let page = self.index.list_recent(SessionListQuery {
            workspace_ids: Some(workspace_ids),
            limit: Some(RECENT_SESSIONS_LIMIT),
            ..Default::default()
        }).await?;
        Ok(page.items)
    }

    async fn count(&self, workspace_id: &str) -> Result<i64, WireAsyncError> {
        let workspace_ids = self.aliases.resolve_alias_ids(workspace_id).await?;
        self.index.count(SessionCountQuery {
            workspace_ids: Some(workspace_ids),
            include_archived: Some(true),
        }).await
    }
}

pub fn register_workspace_sessions_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "workspaceSessions");
}
