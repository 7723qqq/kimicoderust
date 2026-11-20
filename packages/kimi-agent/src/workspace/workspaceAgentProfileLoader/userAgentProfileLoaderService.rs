use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfile;
use crate::app::agent_profile_catalog::agent_profile_contribution::AGENT_PROFILE_SOURCE_PRIORITY;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::IBuiltinAgentProfileLoader;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;

use super::internal::agent_file_discovery::discover_agent_files;
use super::internal::agent_profile_from_file::profiles_from_discovery;
use super::internal::agent_profile_loader::{
    AgentProfileLoaderBase, AgentProfileLoaderHooks, AgentProfileProvide,
};
use super::internal::agent_roots::user_agent_roots;
use super::internal::system_file::load_system_md_profile;
use super::user_agent_profile_loader::IUserAgentProfileLoader;

pub struct UserAgentProfileLoaderService {
    base: Arc<AgentProfileLoaderBase>,
    default_profile: Arc<Mutex<AgentProfile>>,
}

impl UserAgentProfileLoaderService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        fs: Arc<dyn IHostFileSystem>,
        log: Arc<dyn ILogService>,
        builtin: Arc<dyn IBuiltinAgentProfileLoader>,
        workspace: Arc<dyn IWorkspaceContext>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        provide: AgentProfileProvide,
    ) -> Self {
        let default_profile = Arc::new(Mutex::new(builtin.get_default()));
        let load_bootstrap = bootstrap.clone();
        let load_fs = fs.clone();
        let load_log = log.clone();
        let load_builtin = builtin.clone();
        let load_default = default_profile.clone();
        let base = Arc::new(AgentProfileLoaderBase::new(
            log.clone(),
            registry,
            provide,
            AgentProfileLoaderHooks {
                source_id: "user".to_string(),
                priority: AGENT_PROFILE_SOURCE_PRIORITY.user,
                fatal: false,
                workspace_key: Some(workspace.workspace_id()),
                context_key: None,
                load: Arc::new(move || {
                    let bootstrap = load_bootstrap.clone();
                    let fs = load_fs.clone();
                    let log = load_log.clone();
                    let builtin = load_builtin.clone();
                    let default_profile = load_default.clone();
                    async move {
                        let warn_log = log.clone();
                        let roots = user_agent_roots(
                            fs.as_ref(),
                            &bootstrap.home_dir(),
                            &bootstrap.os_home_dir(),
                            Some(&move |message, error| {
                                warn_log.warn(
                                    message,
                                    error.map(|error| {
                                        serde_json::json!({ "error": error.to_string() })
                                    }),
                                );
                            }),
                        )
                        .await?;
                        let system_md = load_system_md_profile(
                            fs.as_ref(),
                            &bootstrap.home_dir(),
                            builtin.get_default(),
                            &|message| log.warn(message, None),
                        )
                        .await?;
                        *default_profile.lock() = system_md
                            .clone()
                            .unwrap_or_else(|| builtin.get_default());
                        let discovery = discover_agent_files(
                            fs.as_ref(),
                            &roots,
                            Some(&|message, _error| log.warn(message, None)),
                        )
                        .await?;
                        let prompt_profile = default_profile.clone();
                        let mut contribution = profiles_from_discovery(
                            discovery,
                            Arc::new(move |context| {
                                (prompt_profile.lock().render_system_prompt)(context)
                            }),
                        );
                        if let Some(system_md) = system_md {
                            contribution.profiles.push(system_md);
                        }
                        Ok(contribution)
                    }
                    .boxed()
                }),
            },
        ));
        base.start();
        Self {
            base,
            default_profile,
        }
    }

    pub fn get_default_profile(&self) -> AgentProfile {
        self.default_profile.lock().clone()
    }
}

impl UserAgentProfileLoaderService {
    pub fn dispose(&self) {
        self.base.dispose();
    }
}

impl IUserAgentProfileLoader for UserAgentProfileLoaderService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.base.ready()
    }

    fn get_default_profile(&self) -> AgentProfile {
        UserAgentProfileLoaderService::get_default_profile(self)
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let base = self.base.clone();
        Box::pin(async move { base.reload().await })
    }
}
