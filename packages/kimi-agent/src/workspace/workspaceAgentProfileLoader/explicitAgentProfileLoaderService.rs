use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_contribution::{
    AGENT_PROFILE_SOURCE_PRIORITY, AgentProfileContribution,
};
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::explicit_agent_profile_loader::IExplicitAgentProfileLoader;
use super::internal::agent_file::{ParseAgentFileOptions, parse_agent_file_text};
use super::internal::agent_profile_from_file::agent_profile_from_file;
use super::internal::agent_profile_loader::{
    AgentProfileLoaderBase, AgentProfileLoaderHooks, AgentProfileProvide,
};
use super::internal::paths::resolve_agent_path;
use super::internal::types::AgentFileSource;
use super::user_agent_profile_loader::IUserAgentProfileLoader;

pub struct ExplicitAgentProfileLoaderService {
    base: Arc<AgentProfileLoaderBase>,
    _subscription: DisposableRef,
}

impl ExplicitAgentProfileLoaderService {
    pub fn new(
        workspace: Arc<dyn IWorkspaceContext>,
        dirs: Arc<dyn IWorkspaceDirs>,
        bootstrap: Arc<dyn IBootstrapService>,
        fs: Arc<dyn IHostFileSystem>,
        log: Arc<dyn ILogService>,
        user: Arc<dyn IUserAgentProfileLoader>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        provide: AgentProfileProvide,
    ) -> Self {
        let load_dirs = dirs.clone();
        let load_bootstrap = bootstrap.clone();
        let load_fs = fs.clone();
        let load_user = user.clone();
        let base = Arc::new(AgentProfileLoaderBase::new(
            log.clone(),
            registry,
            provide,
            AgentProfileLoaderHooks {
                source_id: "explicit".to_string(),
                priority: AGENT_PROFILE_SOURCE_PRIORITY.explicit,
                fatal: true,
                workspace_key: Some(workspace.workspace_id()),
                context_key: None,
                load: Arc::new(move || {
                    let dirs = load_dirs.clone();
                    let bootstrap = load_bootstrap.clone();
                    let fs = load_fs.clone();
                    let user = load_user.clone();
                    async move {
                        let files = bootstrap.args().agent_files.clone().unwrap_or_default();
                        let mut profiles = Vec::new();
                        for file in files {
                            let file_path = resolve_agent_path(
                                &file,
                                &dirs.primary(),
                                &bootstrap.os_home_dir(),
                            );
                            let text = fs.read_text(&file_path, None).await?;
                            let user = user.clone();
                            profiles.push(agent_profile_from_file(
                                parse_agent_file_text(ParseAgentFileOptions {
                                    path: file_path,
                                    source: AgentFileSource::Explicit,
                                    text,
                                })?,
                                Arc::new(move |context| {
                                    (user.get_default_profile().render_system_prompt)(context)
                                }),
                            ));
                        }
                        Ok(AgentProfileContribution {
                            profiles,
                            skipped: None,
                            scanned_roots: None,
                        })
                    }
                    .boxed()
                }),
            },
        ));
        let reload_base = base.clone();
        let subscription = dirs.on_did_change(Arc::new(move || {
            let base = reload_base.clone();
            let log = log.clone();
            tokio::spawn(async move {
                if let Err(error) = base.reload().await {
                    log.warn(
                        &format!("agent profile loader \"explicit\" primary reload failed: {error}"),
                        None,
                    );
                }
            });
        }));
        base.start();
        Self {
            base,
            _subscription: subscription,
        }
    }
}

impl ExplicitAgentProfileLoaderService {
    pub fn dispose(&self) {
        self._subscription.dispose();
        self.base.dispose();
    }
}

impl IExplicitAgentProfileLoader for ExplicitAgentProfileLoaderService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.base.ready()
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let base = self.base.clone();
        Box::pin(async move { base.reload().await })
    }
}
