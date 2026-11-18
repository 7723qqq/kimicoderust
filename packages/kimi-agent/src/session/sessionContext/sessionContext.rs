use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::_base::di::scope::{ScopeSeed, seed_instance};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWorkspaceAssociationSnapshot {
    pub session_id: String,
    pub workspace_id: String,
    pub cwd: String,
}

pub trait ISessionContext: Send + Sync {
    fn session_id(&self) -> String;
    fn workspace_id(&self) -> String;
    fn session_dir(&self) -> String;
    fn meta_scope(&self) -> String;
    fn cwd(&self) -> String;
    fn scope(&self, sub_key: Option<&str>) -> String;
}

pub fn snapshot_session_workspace_association(
    context: &dyn ISessionContext,
) -> SessionWorkspaceAssociationSnapshot {
    SessionWorkspaceAssociationSnapshot {
        session_id: context.session_id(),
        workspace_id: context.workspace_id(),
        cwd: context.cwd(),
    }
}

pub fn session_context_seed(ctx: Arc<dyn ISessionContext>) -> ScopeSeed {
    seed_instance(&I_SESSION_CONTEXT, Arc::new(ctx))
}

#[derive(Debug, Clone)]
pub struct SessionContextInput {
    pub session_id: String,
    pub workspace_id: String,
    pub session_dir: String,
    pub session_scope: String,
    pub cwd: String,
    pub meta_scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    session_id: String,
    workspace_id: String,
    session_dir: String,
    meta_scope: String,
    cwd: String,
    session_scope: String,
}

impl ISessionContext for SessionContext {
    fn session_id(&self) -> String {
        self.session_id.clone()
    }

    fn workspace_id(&self) -> String {
        self.workspace_id.clone()
    }

    fn session_dir(&self) -> String {
        self.session_dir.clone()
    }

    fn meta_scope(&self) -> String {
        self.meta_scope.clone()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn scope(&self, sub_key: Option<&str>) -> String {
        match sub_key.filter(|sub_key| !sub_key.is_empty()) {
            Some(sub_key) => format!("{}/{}", self.session_scope, sub_key),
            None => self.session_scope.clone(),
        }
    }
}

pub fn make_session_context(input: SessionContextInput) -> SessionContext {
    SessionContext {
        session_id: input.session_id,
        workspace_id: input.workspace_id,
        session_dir: input.session_dir,
        meta_scope: input.meta_scope.unwrap_or_else(|| input.session_scope.clone()),
        cwd: input.cwd,
        session_scope: input.session_scope,
    }
}


pub static I_SESSION_CONTEXT: LazyLock<ServiceIdentifier<Arc<dyn ISessionContext>>> =
    LazyLock::new(|| create_decorator("sessionContext"));
