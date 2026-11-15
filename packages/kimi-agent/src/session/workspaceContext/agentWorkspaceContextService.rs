use crate::_base::state::state_registry::IStateRegistryExt;
use std::sync::Arc;

use crate::_base::di::instantiation::LiveRef;
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::session::state::session_state::ISessionStateService;
use crate::session::workspace_context::workspace_context_service::{
    WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY, WORKSPACE_CONTEXT_WORK_DIR_KEY,
};
use crate::wire::record::WireAsyncError;

use super::workspace_context::{ISessionWorkspaceContext, PathAccessOperation};
use super::workspace_paths::{
    WorkspacePathSemantics, assert_workspace_allowed, host_workspace_path_semantics,
    is_within_workspace, resolve_workspace_path,
};

pub struct AgentWorkspaceContextService {
    states: Arc<dyn ISessionStateService>,
    environment: LiveRef<Arc<dyn IAgentEnvironmentService>>,
}

impl AgentWorkspaceContextService {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        environment: LiveRef<Arc<dyn IAgentEnvironmentService>>,
    ) -> Self {
        Self { states, environment }
    }

    fn roots(&self) -> crate::environment::environment::EnvironmentWorkspaceRoots {
        if let Some(environment) = self.environment.current() {
            return environment.workspace_roots();
        }
        crate::environment::environment::EnvironmentWorkspaceRoots {
            work_dir: self.states.get(&WORKSPACE_CONTEXT_WORK_DIR_KEY),
            additional_dirs: Some(self.states.get(&WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY)),
        }
    }

    fn path_semantics(&self) -> WorkspacePathSemantics {
        if let Some(environment) = self.environment.current() {
            if let Some(path) = environment.inspect().path {
                return path;
            }
        }
        host_workspace_path_semantics()
    }
}

impl ISessionWorkspaceContext for AgentWorkspaceContextService {
    fn work_dir(&self) -> String {
        self.roots().work_dir
    }

    fn additional_dirs(&self) -> Vec<String> {
        self.roots().additional_dirs.unwrap_or_default()
    }

    fn set_work_dir(&self, work_dir: String) {
        self.states.set(&WORKSPACE_CONTEXT_WORK_DIR_KEY, work_dir);
    }

    fn resolve(&self, rel: &str) -> String {
        resolve_workspace_path(&self.path_semantics(), &self.work_dir(), rel)
    }

    fn is_within(&self, abs_path: &str) -> bool {
        is_within_workspace(
            &self.path_semantics(),
            &self.work_dir(),
            &self.additional_dirs(),
            abs_path,
        )
    }

    fn assert_allowed(&self, abs_path: &str, op: PathAccessOperation) -> Result<String, WireAsyncError> {
        assert_workspace_allowed(
            &self.path_semantics(),
            &self.work_dir(),
            &self.additional_dirs(),
            abs_path,
            op,
        )
    }
}
