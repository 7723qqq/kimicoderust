use serde::{Deserialize, Serialize};

use crate::_base::di::scope::ScopeSeed;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceSource {
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMeta {
    pub id: String,
    pub root: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub created_at: i64,
    pub last_opened_at: i64,
}

pub trait IWorkspaceContext: Send + Sync {
    fn workspace_id(&self) -> String;
    fn cwd(&self) -> String;
    fn source(&self) -> WorkspaceSource;
    fn remote_cwd(&self) -> Option<String> {
        None
    }
    fn meta(&self) -> WorkspaceMeta;
    fn persistence_scope(&self) -> String;
}

pub fn workspace_context_seed(ctx: impl IWorkspaceContext + Clone + 'static) -> ScopeSeed {
    ScopeSeed::new::<dyn IWorkspaceContext>(ctx)
}
