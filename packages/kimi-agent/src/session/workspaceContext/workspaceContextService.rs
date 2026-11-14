use std::sync::{Arc, LazyLock};

use crate::_base::di::lifecycle::IDisposable;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::session_metadata::session_metadata::ISessionMetadata;
use crate::session::state::session_state::{ISessionStateService, StateDefinition};
use crate::session::workdir::workdir::ISessionWorkdir;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::state::state::define_state;
use crate::wire::record::WireAsyncError;

use super::workspace_context::{ISessionWorkspaceContext, PathAccessOperation};
use super::workspace_paths::{
    assert_workspace_allowed, host_workspace_path_semantics, is_within_workspace,
    resolve_workspace_path,
};
use super::workspace_roots::resolve_effective_workspace_roots;

pub static WORKSPACE_CONTEXT_WORK_DIR_KEY: LazyLock<StateDefinition<String>> =
    LazyLock::new(|| define_state("workspaceContext.workDir", Arc::new(String::new)));
pub static WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY: LazyLock<StateDefinition<Vec<String>>> =
    LazyLock::new(|| define_state("workspaceContext.additionalDirs", Arc::new(Vec::new)));

pub struct SessionWorkspaceContextService {
    states: Arc<dyn ISessionStateService>,
    ctx: Arc<dyn ISessionContext>,
    workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    workdir: Arc<dyn ISessionWorkdir>,
    _subscription: Box<dyn IDisposable>,
}

impl SessionWorkspaceContextService {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        ctx: Arc<dyn ISessionContext>,
        workspace_info: Arc<dyn ISessionWorkspaceInfo>,
        workdir: Arc<dyn ISessionWorkdir>,
    ) -> Self {
        states.contribute_state(&WORKSPACE_CONTEXT_WORK_DIR_KEY);
        states.contribute_state(&WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY);
        let sync_states = states.clone();
        let sync_ctx = ctx.clone();
        let sync_workdir = workdir.clone();
        let subscription = workspace_info.on_did_change(Arc::new(move || {
            sync_roots(&sync_states, &sync_ctx, &sync_workdir);
        }));
        let this = Self {
            states,
            ctx,
            workspace_info: workspace_info.clone(),
            workdir,
            _subscription: subscription,
        };
        this.sync_roots();
        let _ = &this._subscription;
        this
    }

    fn sync_roots(&self) {
        sync_roots(&self.states, &self.ctx, &self.workspace_info, &self.workdir);
    }

    fn current_work_dir(&self) -> String {
        self.states.get(&WORKSPACE_CONTEXT_WORK_DIR_KEY)
    }

    fn current_additional_dirs(&self) -> Vec<String> {
        self.states.get(&WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY)
    }
}

fn sync_roots(
    states: &Arc<dyn ISessionStateService>,
    ctx: &Arc<dyn ISessionContext>,
    workspace_info: &Arc<dyn ISessionWorkspaceInfo>,
    workdir: &Arc<dyn ISessionWorkdir>,
) {
    let roots = resolve_effective_workspace_roots(
        ctx.as_ref(),
        Some(&crate::environment::environment::EnvironmentBinding {
            environment_id: workdir.environment_id(),
            cwd: workdir.cwd(),
        }),
        workspace_info.as_ref(),
        None,
    );
    states.set(&WORKSPACE_CONTEXT_WORK_DIR_KEY, roots.work_dir);
    states.set(&WORKSPACE_CONTEXT_ADDITIONAL_DIRS_KEY, roots.additional_dirs);
}

impl ISessionWorkspaceContext for SessionWorkspaceContextService {
    fn work_dir(&self) -> String {
        self.current_work_dir()
    }

    fn additional_dirs(&self) -> Vec<String> {
        self.current_additional_dirs()
    }

    fn set_work_dir(&self, work_dir: String) {
        self.states.set(&WORKSPACE_CONTEXT_WORK_DIR_KEY, work_dir);
    }

    fn resolve(&self, rel: &str) -> String {
        resolve_workspace_path(&host_workspace_path_semantics(), &self.current_work_dir(), rel)
    }

    fn is_within(&self, abs_path: &str) -> bool {
        is_within_workspace(
            &host_workspace_path_semantics(),
            &self.current_work_dir(),
            &self.current_additional_dirs(),
            abs_path,
        )
    }

    fn assert_allowed(&self, abs_path: &str, op: PathAccessOperation) -> Result<String, WireAsyncError> {
        assert_workspace_allowed(
            &host_workspace_path_semantics(),
            &self.current_work_dir(),
            &self.current_additional_dirs(),
            abs_path,
            op,
        )
    }
}

#[allow(dead_code)]
fn _metadata_marker(_: &dyn ISessionMetadata) {}
